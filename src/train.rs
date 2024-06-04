use std::time;

use anyhow::Result;
use burn::lr_scheduler::linear::{LinearLrScheduler, LinearLrSchedulerConfig};
use burn::lr_scheduler::LrScheduler;
use burn::nn::loss::{HuberLossConfig, MseLoss};
use burn::optim::adaptor::OptimizerAdaptor;
use burn::optim::Adam;
use burn::tensor::{Bool, Distribution, ElementConversion};
use burn::{
    config::Config,
    optim::{AdamConfig, GradientsParams, Optimizer},
    tensor::Tensor,
};
use ndarray::Array;
use rand::{rngs::StdRng, SeedableRng};
use tracing::info_span;

use crate::scene::SceneBatch;
use crate::splat_render::sync_span::SyncSpan;
use crate::splat_render::{self, AutodiffBackend, RenderAux};
use crate::utils::quaternion_rotation;
use crate::{gaussian_splats::Splats, utils};

#[derive(Config)]
pub(crate) struct LrConfig {
    #[config(default = 3e-3)]
    min_lr: f64,
    #[config(default = 3e-3)]
    max_lr: f64,
}

#[derive(Config)]
pub(crate) struct TrainConfig {
    pub lr_mean: LrConfig,
    pub lr_opac: LrConfig,
    pub lr_rest: LrConfig,
    pub scene_path: String,

    #[config(default = 42)]
    pub(crate) seed: u64,
    #[config(default = 400)]
    pub(crate) warmup_steps: u32,
    #[config(default = 150)]
    pub(crate) refine_every: u32,

    #[config(default = 0.0)]
    pub(crate) ssim_weight: f32,
    // threshold of opacity for culling gaussians. One can set it to a lower value (e.g. 0.005) for higher quality."""
    #[config(default = 0.05)]
    pub(crate) prune_alpha_thresh: f32,
    #[config(default = 0.005)]
    pub(crate) prune_scale_thresh: f32,

    #[config(default = 0.00005)]
    pub(crate) clone_split_grad_threshold: f32,
    #[config(default = 0.01)]
    pub(crate) split_clone_size_threshold: f32,
    // threshold of scale for culling huge gaussians.
    #[config(default = 0.5)]
    pub(crate) cull_scale_thresh: f32,
    #[config(default = 0.15)]
    pub(crate) cull_screen_size: f32,
    #[config(default = 30)]
    pub(crate) reset_alpha_every: u32,
    #[config(default = 10000)]
    pub(crate) train_steps: u32,
    #[config(default = false)]
    pub(crate) random_bck_color: bool,
    #[config(default = 100)]
    pub visualize_every: u32,
    #[config(default = 250)]
    pub visualize_splats_every: u32,
}

struct TrainStepStats<B: AutodiffBackend> {
    pred_image: Tensor<B, 3>,
    loss: Tensor<B, 1>,
    psnr: Tensor<B, 1>,
    aux: crate::splat_render::RenderAux<B>,
}

pub struct SplatTrainer<B: AutodiffBackend>
where
    B::InnerBackend: splat_render::Backend,
{
    config: TrainConfig,

    rng: StdRng,

    sched_mean: LinearLrScheduler,
    sched_opac: LinearLrScheduler,
    sched_rest: LinearLrScheduler,

    opt_config: AdamConfig,

    optim: OptimizerAdaptor<Adam<B::InnerBackend>, Splats<B>, B>,

    iter: u32,

    // Maximum projected radius of each Gaussian in pixel-units. It is
    // later used during culling.
    max_radii_2d: Tensor<B, 1>,

    // Helper tensors for accumulating the viewspace_xy gradients and the number
    // of observations per gaussian. Used in pruning and densification.
    xy_grad_norm_accum: Tensor<B, 1>,
}

impl<B: AutodiffBackend> SplatTrainer<B>
where
    B::InnerBackend: splat_render::Backend,
{
    pub fn new(num_points: usize, config: &TrainConfig, splats: &Splats<B>) -> Self {
        let opt_config = AdamConfig::new().with_epsilon(1e-15);
        let optim = opt_config.init::<B, Splats<B>>();

        let device = &splats.means.device();

        let sched_mean = LinearLrSchedulerConfig::new(
            config.lr_mean.max_lr,
            config.lr_mean.min_lr,
            config.train_steps as usize,
        )
        .init();
        let sched_opac = LinearLrSchedulerConfig::new(
            config.lr_opac.max_lr,
            config.lr_opac.min_lr,
            config.train_steps as usize,
        )
        .init();
        let sched_rest = LinearLrSchedulerConfig::new(
            config.lr_rest.max_lr,
            config.lr_rest.min_lr,
            config.train_steps as usize,
        )
        .init();

        Self {
            config: config.clone(),
            rng: StdRng::from_seed([10; 32]),
            iter: 0,
            optim,
            opt_config,
            sched_mean,
            sched_opac,
            sched_rest,
            max_radii_2d: Tensor::zeros([num_points], device),
            xy_grad_norm_accum: Tensor::zeros([num_points], device),
        }
    }

    fn reset_stats(&mut self, num_points: usize, device: &B::Device) {
        self.max_radii_2d = Tensor::zeros([num_points], device);
        self.xy_grad_norm_accum = Tensor::zeros([num_points], device);
    }

    // Updates rolling statistics that we capture during rendering.
    pub(crate) fn update_stats(&mut self, aux: &RenderAux<B>, xys_grad: Tensor<B, 2>) {
        let radii = Tensor::zeros_like(&aux.radii_compact).select_assign(
            0,
            aux.global_from_compact_gid.clone(),
            aux.radii_compact.clone(),
        );

        self.max_radii_2d = Tensor::max_pair(self.max_radii_2d.clone(), radii.clone());

        self.xy_grad_norm_accum = Tensor::max_pair(
            self.xy_grad_norm_accum.clone(),
            xys_grad
                .clone()
                .mul(xys_grad.clone())
                .sum_dim(1)
                .squeeze(1)
                .sqrt(),
        );
    }

    // Densifies and prunes the Gaussians.
    pub fn densify_and_prune(
        &mut self,
        splats: &mut Splats<B>,
        grad_threshold: f32,
        max_pixel_threshold: Option<f32>,
        max_world_size_threshold: Option<f32>,
        clone_vs_split_size_threshold: f32,
        device: &B::Device,
    ) {
        if let Some(threshold) = max_pixel_threshold {
            // Delete Gaussians with too large of a radius in pixel-units.
            let big_splats_mask = self.max_radii_2d.clone().greater_elem(threshold);
            self.prune_points(splats, big_splats_mask)
        }

        if let Some(threshold) = max_world_size_threshold {
            // Delete Gaussians with too large of a radius in world-units.
            let prune_mask = splats
                .log_scales
                .val()
                .exp()
                .max_dim(1)
                .squeeze(1)
                .greater_elem(threshold);
            self.prune_points(splats, prune_mask);
        }

        // Compute average magnitude of the gradient for each Gaussian in
        // pixel-units while accounting for the number of times each Gaussian was
        // seen during training.
        let grads = self.xy_grad_norm_accum.clone();

        let big_grad_mask = grads.greater_equal_elem(grad_threshold);
        let split_clone_size_mask = splats
            .log_scales
            .val()
            .exp()
            .max_dim(1)
            .squeeze(1)
            .lower_elem(clone_vs_split_size_threshold);

        let clone_mask = Tensor::stack::<2>(
            vec![split_clone_size_mask.clone(), big_grad_mask.clone()],
            1,
        )
        .all_dim(1)
        .squeeze::<1>(1);
        let split_mask =
            Tensor::stack::<2>(vec![split_clone_size_mask.bool_not(), big_grad_mask], 1)
                .all_dim(1)
                .squeeze::<1>(1);

        // Need to be very careful not to do any operations with this tensor, as it might be
        // less than the minimum size wgpu can support :/
        let clone_where = clone_mask.clone().argwhere();

        if clone_where.dims()[0] >= 4 {
            let clone_inds = clone_where.squeeze(1);

            let new_means = splats.means.val().select(0, clone_inds.clone());
            let new_rots = splats.rotation.val().select(0, clone_inds.clone());
            let new_coeffs = splats.sh_coeffs.val().select(0, clone_inds.clone());
            let new_opac = splats.raw_opacity.val().select(0, clone_inds.clone());
            let new_scales = splats.log_scales.val().select(0, clone_inds.clone());
            splats.concat_splats(new_means, new_rots, new_coeffs, new_opac, new_scales);
        }

        let split_where = split_mask.clone().argwhere();
        if split_where.dims()[0] >= 4 {
            let split_inds = split_where.squeeze(1);
            let samps = split_inds.dims()[0];

            let centered_samples =
                Tensor::random([samps * 2, 3], Distribution::Normal(0.0, 1.0), device);
            let scaled_samples = splats
                .log_scales
                .val()
                .select(0, split_inds.clone())
                .repeat(0, 2)
                .exp()
                * centered_samples;

            // Remove original points we're splitting.
            // TODO: Could just replace them? Maybe?
            let splits = 2;

            let rotated_samples = quaternion_rotation(
                scaled_samples,
                splats
                    .rotation
                    .val()
                    .select(0, split_inds.clone())
                    .repeat(0, splits),
            );
            // TODO: Rotate samples
            let new_means = rotated_samples
                + splats
                    .means
                    .val()
                    .select(0, split_inds.clone())
                    .repeat(0, splits);
            let new_rots = splats
                .rotation
                .val()
                .select(0, split_inds.clone())
                .repeat(0, splits);
            let new_coeffs = splats
                .sh_coeffs
                .val()
                .select(0, split_inds.clone())
                .repeat(0, splits);
            let new_opac = splats
                .raw_opacity
                .val()
                .select(0, split_inds.clone())
                .repeat(0, splits);
            let new_scales = (splats.log_scales.val().select(0, split_inds.clone()).exp() / 1.6)
                .log()
                .repeat(0, splits);
            self.prune_points(splats, split_mask.clone());

            splats.concat_splats(new_means, new_rots, new_coeffs, new_opac, new_scales);
        }
    }

    pub(crate) fn reset_opacity(&self, splats: &mut Splats<B>) {
        splats.raw_opacity = splats
            .raw_opacity
            .clone()
            .map(|x| Tensor::from_inner((x - 1.0).inner()).require_grad());
    }

    // Prunes points based on the given mask.
    //
    // Args:
    //   mask: bool[n]. If True, prune this Gaussian.
    pub fn prune_points(&mut self, splats: &mut Splats<B>, prune: Tensor<B, 1, Bool>) {
        // bool[n]. If True, delete these Gaussians.
        let valid_inds = prune.bool_not().argwhere().squeeze(1);

        let start_splats = splats.num_splats();
        let new_points = valid_inds.dims()[0];

        if new_points < start_splats {
            self.max_radii_2d = self.max_radii_2d.clone().select(0, valid_inds.clone());
            self.xy_grad_norm_accum = self
                .xy_grad_norm_accum
                .clone()
                .select(0, valid_inds.clone());

            splats.means = splats.means.clone().map(|x| {
                Tensor::from_inner(x.select(0, valid_inds.clone()).inner()).require_grad()
            });
            splats.sh_coeffs = splats.sh_coeffs.clone().map(|x| {
                Tensor::from_inner(x.select(0, valid_inds.clone()).inner()).require_grad()
            });
            splats.rotation = splats.rotation.clone().map(|x| {
                Tensor::from_inner(x.select(0, valid_inds.clone()).inner()).require_grad()
            });
            splats.raw_opacity = splats.raw_opacity.clone().map(|x| {
                Tensor::from_inner(x.select(0, valid_inds.clone()).inner()).require_grad()
            });
            splats.log_scales = splats.log_scales.clone().map(|x| {
                Tensor::from_inner(x.select(0, valid_inds.clone()).inner()).require_grad()
            });
        }
    }

    // TODO: Probably want to feed in a batch of data here.
    pub fn step(
        &mut self,
        batch: SceneBatch<B>,
        splats: Splats<B>,
        rec: &rerun::RecordingStream,
    ) -> Result<Splats<B>, anyhow::Error> {
        let _span = info_span!("Train step").entered();

        let start_time = time::Instant::now();

        let device = &splats.means.device();

        let camera = &batch.camera;
        let view_image = &batch.gt_image;

        let background_color = if self.config.random_bck_color {
            glam::vec3(rand::random(), rand::random(), rand::random())
        } else {
            glam::Vec3::ZERO
        };

        let img_size = glam::uvec2(view_image.dims()[1] as u32, view_image.dims()[0] as u32);
        let (pred_image, aux) = splats.render(camera, img_size, background_color, false);
        let dims = pred_image.dims();

        let calc_losses = SyncSpan::<B>::new("Calculate losses", device);

        // There might be some marginal benefit to caching the "loss objects". I wish Burn had a more
        // functional style for this.
        let mse = MseLoss::new().forward(
            pred_image.clone(),
            batch.gt_image.clone(),
            burn::nn::loss::Reduction::Mean,
        );
        let huber = HuberLossConfig::new(0.05).init::<B>(device);
        let l1_loss = huber.forward(
            pred_image.clone(),
            batch.gt_image.clone(),
            burn::nn::loss::Reduction::Mean,
        );
        let mut loss = l1_loss;

        if self.config.ssim_weight > 0.0 {
            let pred_rgb = pred_image.clone().slice([0..dims[0], 0..dims[1], 0..3]);
            let gt_rgb = batch.gt_image.clone().slice([0..dims[0], 0..dims[1], 0..3]);

            let ssim_loss = crate::ssim::ssim(
                pred_rgb.clone().permute([2, 0, 1]).unsqueeze_dim(3),
                gt_rgb.clone().permute([2, 0, 1]).unsqueeze_dim(3),
                11,
            );

            loss = loss * (1.0 - self.config.ssim_weight)
                + (-ssim_loss + 1.0) * self.config.ssim_weight;
        }
        let psnr = mse.clone().recip().log() * 10.0 / std::f32::consts::LN_10;
        drop(calc_losses);

        let backward_pass = SyncSpan::<B>::new("Backward pass", device);
        let mut grads = loss.backward();
        drop(backward_pass);

        let step_span = SyncSpan::<B>::new("Optimizer step", device);

        // Burn doesn't have a great way to use multiple different learning rates
        // or different optimizers. The current best way seems to be to "distribute" the gradients
        // to different GradientParams. Basically each optimizer step call only sees a
        // a subset of parameter gradients.
        let xys_grad = Tensor::from_inner(splats.xys_dummy.grad_remove(&mut grads).unwrap());

        let mut grad_means = GradientsParams::new();
        grad_means.register(
            splats.means.clone().consume().0,
            splats.means.grad_remove(&mut grads).unwrap(),
        );

        let mut grad_opac = GradientsParams::new();
        grad_opac.register(
            splats.raw_opacity.clone().consume().0,
            splats.raw_opacity.grad_remove(&mut grads).unwrap(),
        );

        let mut grad_rest = GradientsParams::new();
        grad_rest.register(
            splats.sh_coeffs.clone().consume().0,
            splats.sh_coeffs.grad_remove(&mut grads).unwrap(),
        );
        grad_rest.register(
            splats.rotation.clone().consume().0,
            splats.rotation.grad_remove(&mut grads).unwrap(),
        );
        grad_rest.register(
            splats.log_scales.clone().consume().0,
            splats.log_scales.grad_remove(&mut grads).unwrap(),
        );

        // There's an annoying issue in Burn where the scheduler step
        // is a trait function, which requires the backen to be known,
        // which is otherwise unconstrained, leading to needing this ugly call.
        let lr_mean = LrScheduler::<B>::step(&mut self.sched_mean);
        let lr_opac = LrScheduler::<B>::step(&mut self.sched_opac);
        let lr_rest = LrScheduler::<B>::step(&mut self.sched_rest);

        // Now step each optimizer
        let mut splats = splats;
        splats = self.optim.step(lr_mean, splats, grad_means);
        splats = self.optim.step(lr_opac, splats, grad_opac);
        splats = self.optim.step(lr_rest, splats, grad_rest);

        drop(step_span);

        {
            let _norm_rot_span = SyncSpan::<B>::new("Housekeeping", device);
            splats.norm_rotations();
            self.update_stats(&aux, xys_grad);
        }

        if self.iter % self.config.refine_every == 0 {
            // Remove barely visible gaussians.
            let prule_alpha_thresh = self.config.prune_alpha_thresh;
            let alpha_mask = burn::tensor::activation::sigmoid(splats.raw_opacity.val())
                .lower_elem(prule_alpha_thresh);
            self.prune_points(&mut splats, alpha_mask);

            let prune_scale_thresh = self.config.prune_scale_thresh;
            let scale_mask = splats
                .log_scales
                .val()
                .exp()
                .max_dim(1)
                .squeeze(1)
                .lower_elem(prune_scale_thresh);
            self.prune_points(&mut splats, scale_mask);

            if self.iter > self.config.warmup_steps {
                self.densify_and_prune(
                    &mut splats,
                    self.config.clone_split_grad_threshold / (img_size.x.max(img_size.y) as f32),
                    Some(self.config.cull_screen_size * (dims[0].max(dims[1]) as f32)),
                    Some(self.config.cull_scale_thresh),
                    self.config.split_clone_size_threshold,
                    device,
                );

                if self.iter % (self.config.refine_every * self.config.reset_alpha_every) == 0 {
                    self.reset_opacity(&mut splats);
                }
            }

            self.reset_stats(splats.num_splats(), device);
            self.optim = self.opt_config.init::<B, Splats<B>>();
        }

        let stats = TrainStepStats {
            aux,
            loss,
            psnr,
            pred_image,
        };

        rec.set_time_sequence("iterations", self.iter);

        rec.log("lr/mean", &rerun::Scalar::new(lr_mean))?;
        rec.log("lr/opac", &rerun::Scalar::new(lr_opac))?;
        rec.log("lr/rest", &rerun::Scalar::new(lr_rest))?;

        rec.log(
            "splats/num",
            &rerun::Scalar::new(splats.num_splats() as f64).clone(),
        )?;

        rec.log(
            "performance/step_ms",
            &rerun::Scalar::new((time::Instant::now() - start_time).as_secs_f64() * 1000.0).clone(),
        )?;

        if self.iter % self.config.visualize_every == 0 {
            self.visualize_train_stats(rec, stats)?;

            let gt_image = Array::from_shape_vec(
                batch.gt_image.dims(),
                batch.gt_image.to_data().convert::<f32>().value,
            )?
            .map(|x| (*x * 255.0).clamp(0.0, 255.0) as u8);

            rec.log(
                "images/ground truth",
                &rerun::Image::try_from(gt_image.to_owned())?,
            )?;
        }

        if self.iter % self.config.visualize_splats_every == 0 {
            splats.visualize(rec)?;
        }

        self.iter += 1;

        Ok(splats)
    }

    fn visualize_train_stats(
        &self,
        rec: &rerun::RecordingStream,
        stats: TrainStepStats<B>,
    ) -> Result<(), anyhow::Error> {
        rec.log(
            "losses/main",
            &rerun::Scalar::new(utils::burn_to_scalar(stats.loss).elem::<f64>()),
        )?;
        rec.log(
            "stats/PSNR",
            &rerun::Scalar::new(utils::burn_to_scalar(stats.psnr).elem::<f64>()),
        )?;

        rec.log(
            "splats/num_intersects",
            &rerun::Scalar::new(utils::burn_to_scalar(stats.aux.num_intersects).elem::<f64>()),
        )?;
        rec.log(
            "splats/num_visible",
            &rerun::Scalar::new(utils::burn_to_scalar(stats.aux.num_visible).elem::<f64>()),
        )?;

        let tile_bins = stats.aux.tile_bins;
        let tile_size = tile_bins.dims();
        let tile_depth = tile_bins
            .clone()
            .slice([0..tile_size[0], 0..tile_size[1], 1..2])
            - tile_bins
                .clone()
                .slice([0..tile_size[0], 0..tile_size[1], 0..1]);

        rec.log(
            "images/tile depth",
            &rerun::Tensor::try_from(Array::from_shape_vec(
                tile_depth.dims(),
                tile_depth.to_data().convert::<u32>().value,
            )?)?,
        )?;

        let pred_image = Array::from_shape_vec(
            stats.pred_image.dims(),
            stats.pred_image.to_data().convert::<f32>().value,
        )?
        .map(|x| (*x * 255.0).clamp(0.0, 255.0) as u8);

        rec.log("images/predicted", &rerun::Image::try_from(pred_image)?)?;

        Ok(())
    }
}
