use anyhow::Result;
use brush_render::gaussian_splats::{RandomSplatsConfig, Splats};
use brush_render::{AutodiffBackend, Backend, RenderAux};
use burn::lr_scheduler::exponential::{ExponentialLrScheduler, ExponentialLrSchedulerConfig};
use burn::lr_scheduler::LrScheduler;
use burn::optim::adaptor::OptimizerAdaptor;
use burn::optim::Adam;
use burn::tensor::{Bool, Distribution, Int};
use burn::{
    config::Config,
    optim::{AdamConfig, GradientsParams, Optimizer},
    tensor::Tensor,
};
use tracing::info_span;

use crate::scene::SceneBatch;

#[derive(Config)]
pub struct TrainConfig {
    // period of steps where refinement is turned off
    #[config(default = 500)]
    warmup_steps: u32,

    // period of steps where gaussians are culled and densified
    #[config(default = 100)]
    refine_every: u32,

    // threshold of opacity for culling gaussians. One can set it to a lower value (e.g. 0.005) for higher quality
    #[config(default = 0.1)]
    cull_alpha_thresh: f32,

    // threshold of scale for culling huge gaussians
    #[config(default = 0.5)]
    cull_scale_thresh: f32,

    // Every this many refinement steps, reset the alpha
    #[config(default = 30)]
    reset_alpha_every: u32,

    // threshold of positional gradient norm for densifying gaussians
    // TODO: Abs grad.
    #[config(default = 0.0001)]
    densify_grad_thresh: f32,

    // below this size, gaussians are *duplicated*, otherwise split.
    #[config(default = 0.01)]
    densify_size_thresh: f32,

    // Whether to render images with a random background color.
    #[config(default = false)]
    pub(crate) random_bck_color: bool,

    #[config(default = 0.0)]
    ssim_weight: f32,

    // TODO: Add a resolution schedule.

    // Learning rates.
    lr_mean: ExponentialLrSchedulerConfig,
    #[config(default = 0.0025)]
    lr_coeffs: f64,

    #[config(default = 0.05)]
    lr_opac: f64,

    #[config(default = 0.005)]
    lr_scale: f64,

    #[config(default = 0.001)]
    lr_rotation: f64,

    #[config(default = 5000)]
    schedule_steps: u32,

    #[config(default = 42)]
    seed: u64,

    #[config(default = 100)]
    visualize_every: u32,

    #[config(default = 250)]
    visualize_splats_every: u32,

    pub initial_model_config: RandomSplatsConfig,
}

pub struct TrainStepStats<B: AutodiffBackend> {
    pub pred_images: Tensor<B, 4>,
    pub auxes: Vec<RenderAux>,
    pub loss: Tensor<B, 1>,
    pub lr_mean: f64,
    pub iter: u32,
}

pub struct SplatTrainer<B: AutodiffBackend>
where
    B::InnerBackend: Backend,
{
    pub iter: u32,

    config: TrainConfig,

    sched_mean: ExponentialLrScheduler,
    optim: OptimizerAdaptor<Adam<B::InnerBackend>, Splats<B>, B>,
    opt_config: AdamConfig,

    // Helper tensors for accumulating the viewspace_xy gradients and the number
    // of observations per gaussian. Used in pruning and densification.
    grad_2d_accum: Tensor<B, 1>,
    xy_grad_counts: Tensor<B, 1, Int>,
}

pub(crate) fn quaternion_rotation<B: Backend>(
    quaternions: Tensor<B, 2>,
    vectors: Tensor<B, 2>,
) -> Tensor<B, 2> {
    let num = vectors.dims()[0];

    let w = quaternions.clone().slice([0..num, 0..1]);
    let x = quaternions.clone().slice([0..num, 1..2]);
    let y = quaternions.clone().slice([0..num, 2..3]);
    let z = quaternions.clone().slice([0..num, 3..4]);

    let vx = vectors.clone().slice([0..num, 0..1]);
    let vy = vectors.clone().slice([0..num, 1..2]);
    let vz = vectors.clone().slice([0..num, 2..3]);

    Tensor::cat(
        vec![
            w.clone() * vx.clone() + y.clone() * vz.clone() - z.clone() * vy.clone(),
            w.clone() * vy.clone() - x.clone() * vz.clone() + z.clone() * vx.clone(),
            w.clone() * vz.clone() + x.clone() * vy.clone() - y.clone() * vx.clone(),
        ],
        1,
    )
}

impl<B: AutodiffBackend> SplatTrainer<B>
where
    B::InnerBackend: Backend,
{
    pub fn new(num_points: usize, config: &TrainConfig, splats: &Splats<B>) -> Self {
        let opt_config = AdamConfig::new().with_epsilon(1e-15);
        let optim = opt_config.init::<B, Splats<B>>();

        let device = &splats.means.device();

        Self {
            config: config.clone(),
            iter: 0,
            sched_mean: config.lr_mean.init(),
            optim,
            opt_config,
            grad_2d_accum: Tensor::zeros([num_points], device),
            xy_grad_counts: Tensor::zeros([num_points], device),
        }
    }

    fn reset_stats(&mut self, num_points: usize, device: &B::Device) {
        self.grad_2d_accum = Tensor::zeros([num_points], device);
        self.xy_grad_counts = Tensor::zeros([num_points], device);
    }

    pub(crate) fn reset_opacity(&self, splats: &mut Splats<B>) {
        Splats::map_param(&mut splats.raw_opacity, |op| {
            Tensor::zeros_like(&op) + self.config.cull_alpha_thresh * 2.0
        });
    }

    // Prunes points based on the given mask.
    //
    // Args:
    //   mask: bool[n]. If True, prune this Gaussian.
    pub async fn prune_points(&mut self, splats: &mut Splats<B>, prune: Tensor<B, 1, Bool>) {
        // TODO: if this prunes all points, burn panics.
        //
        // bool[n]. If True, delete these Gaussians.
        let prune_count = prune.dims()[0];

        if prune_count == 0 {
            return;
        }

        let prune = if prune_count < splats.num_splats() {
            Tensor::cat(
                vec![
                    prune,
                    Tensor::<B, 1>::zeros(
                        [splats.num_splats() - prune_count],
                        &splats.means.device(),
                    )
                    .bool(),
                ],
                0,
            )
        } else {
            prune
        };

        let valid_inds = prune.bool_not().argwhere_async().await.squeeze(1);

        let start_splats = splats.num_splats();
        let new_points = valid_inds.dims()[0];

        if new_points < start_splats {
            self.grad_2d_accum = self.grad_2d_accum.clone().select(0, valid_inds.clone());

            self.xy_grad_counts = self.xy_grad_counts.clone().select(0, valid_inds.clone());

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

    pub async fn step(
        &mut self,
        batch: SceneBatch<B>,
        splats: Splats<B>,
    ) -> Result<(Splats<B>, TrainStepStats<B>), anyhow::Error> {
        let device = &splats.means.device();
        let _span = info_span!("Train step").entered();

        let background_color = if self.config.random_bck_color {
            glam::vec3(rand::random(), rand::random(), rand::random())
        } else {
            glam::Vec3::ZERO
        };

        let [batch_size, img_h, img_w, _] = batch.gt_images.dims();

        let (pred_images, auxes, loss) = {
            let mut renders = vec![];
            let mut auxes = vec![];

            for i in 0..batch.cameras.len() {
                let camera = &batch.cameras[i];

                let (pred_image, aux) = splats.render(
                    camera,
                    glam::uvec2(img_w as u32, img_h as u32),
                    background_color,
                    false,
                );

                renders.push(pred_image);
                auxes.push(aux);
            }

            // TODO: Could probably handle this in Burn.
            let pred_images = if renders.len() == 1 {
                renders[0].clone().reshape([1, img_h, img_w, 4])
            } else {
                Tensor::stack(renders, 0)
            };

            let _span = info_span!("Calculate losses", sync_burn = true).entered();

            let loss = (pred_images.clone() - batch.gt_images.clone()).abs().mean();
            let loss = if self.config.ssim_weight > 0.0 {
                let pred_rgb = pred_images
                    .clone()
                    .slice([0..batch_size, 0..img_h, 0..img_w, 0..3]);
                let gt_rgb =
                    batch
                        .gt_images
                        .clone()
                        .slice([0..batch_size, 0..img_h, 0..img_w, 0..3]);
                let ssim_loss = crate::ssim::ssim(
                    pred_rgb.clone().permute([0, 3, 1, 2]),
                    gt_rgb.clone().permute([0, 3, 1, 2]),
                    11,
                );
                loss * (1.0 - self.config.ssim_weight)
                    + (-ssim_loss + 1.0) * self.config.ssim_weight
            } else {
                loss
            };

            (pred_images, auxes, loss)
        };

        let mut grads = info_span!("Backward pass", sync_burn = true).in_scope(|| loss.backward());

        let mut splats = info_span!("Optimizer step", sync_burn = true).in_scope(|| {
            let mut splats = splats;
            let mut grad_means = GradientsParams::new();
            grad_means.register(
                splats.means.id.clone(),
                splats.means.grad_remove(&mut grads).unwrap(),
            );
            splats = self.optim.step(self.sched_mean.step(), splats, grad_means);

            let mut grad_opac = GradientsParams::new();
            grad_opac.register(
                splats.raw_opacity.id.clone(),
                splats.raw_opacity.grad_remove(&mut grads).unwrap(),
            );
            splats = self.optim.step(self.config.lr_opac, splats, grad_opac);

            let mut grad_coeff = GradientsParams::new();
            grad_coeff.register(
                splats.sh_coeffs.id.clone(),
                splats.sh_coeffs.grad_remove(&mut grads).unwrap(),
            );
            splats = self.optim.step(self.config.lr_coeffs, splats, grad_coeff);

            let mut grad_rot = GradientsParams::new();
            grad_rot.register(
                splats.rotation.id.clone(),
                splats.rotation.grad_remove(&mut grads).unwrap(),
            );
            splats = self.optim.step(self.config.lr_rotation, splats, grad_rot);

            let mut grad_scale = GradientsParams::new();
            grad_scale.register(
                splats.log_scales.id.clone(),
                splats.log_scales.grad_remove(&mut grads).unwrap(),
            );
            splats = self.optim.step(self.config.lr_scale, splats, grad_scale);
            splats
        });

        info_span!("Housekeeping", sync_burn = true).in_scope(|| {
            splats.norm_rotations();

            let xys_grad = Tensor::from_inner(
                splats
                    .xys_dummy
                    .grad_remove(&mut grads)
                    .expect("XY gradients need to be calculated."),
            );

            // From normalized to pixels.
            let xys_grad = xys_grad
                * Tensor::<_, 1>::from_floats([img_w as f32 / 2.0, img_h as f32 / 2.0], device)
                    .reshape([1, 2]);

            let grad_mag = xys_grad.powf_scalar(2.0).sum_dim(1).squeeze(1).sqrt();

            // TODO: Is max of grad better?
            // TODO: Add += to Burn.
            if self.iter > self.config.warmup_steps {
                self.grad_2d_accum = self.grad_2d_accum.clone() + grad_mag.clone();
                self.xy_grad_counts =
                    self.xy_grad_counts.clone() + grad_mag.greater_elem(0.0).int();
            }
        });

        if self.iter > self.config.warmup_steps && self.iter % self.config.refine_every == 0 {
            // Remove barely visible gaussians.
            let alpha_mask = burn::tensor::activation::sigmoid(splats.raw_opacity.val())
                .lower_elem(self.config.cull_alpha_thresh);
            self.prune_points(&mut splats, alpha_mask).await;

            // Delete Gaussians with too large of a radius in world-units.
            let scale_mask = splats
                .log_scales
                .val()
                .exp()
                .max_dim(1)
                .squeeze(1)
                .greater_elem(self.config.cull_scale_thresh);
            self.prune_points(&mut splats, scale_mask).await;

            let grads =
                self.grad_2d_accum.clone() / self.xy_grad_counts.clone().clamp_min(1).float();

            let big_grad_mask = grads.greater_equal_elem(self.config.densify_grad_thresh);

            let split_clone_size_mask = splats
                .log_scales
                .val()
                .exp()
                .max_dim(1)
                .squeeze(1)
                .lower_elem(self.config.densify_size_thresh);

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

            let clone_where = clone_mask.clone().argwhere_async().await;
            tracing::info!("Cloning {} gaussians", clone_where.dims()[0]);

            if clone_where.dims()[0] > 0 {
                let clone_inds = clone_where.squeeze(1);
                let new_means = splats.means.val().select(0, clone_inds.clone());
                let new_rots = splats.rotation.val().select(0, clone_inds.clone());
                let new_coeffs = splats.sh_coeffs.val().select(0, clone_inds.clone());
                let new_opac = splats.raw_opacity.val().select(0, clone_inds.clone());
                let new_scales = splats.log_scales.val().select(0, clone_inds.clone());
                splats.concat_splats(new_means, new_rots, new_coeffs, new_opac, new_scales);
            }

            let split_where = split_mask.clone().argwhere_async().await;
            tracing::info!("Splitting {} gaussians", split_where.dims()[0]);

            if split_where.dims()[0] > 0 {
                let split_inds = split_where.squeeze(1);
                let samps = split_inds.dims()[0];

                let cur_pos = splats.means.val().select(0, split_inds.clone());
                let cur_rots = splats.rotation.val().select(0, split_inds.clone());
                let cur_coeff = splats.sh_coeffs.val().select(0, split_inds.clone());
                let cur_opac = splats.raw_opacity.val().select(0, split_inds.clone());
                let cur_scale = splats.log_scales.val().select(0, split_inds.clone()).exp();

                let samples = quaternion_rotation(
                    cur_rots.clone(),
                    Tensor::random([samps, 3], Distribution::Normal(0.0, 1.0), device)
                        * cur_scale.clone(),
                );

                let new_means = Tensor::cat(
                    vec![cur_pos.clone() + samples.clone(), cur_pos - samples],
                    0,
                );
                let new_rots = cur_rots.repeat_dim(0, 2);
                let new_coeffs = cur_coeff.repeat_dim(0, 2);
                let new_opac = cur_opac.repeat_dim(0, 2);
                let new_scales = (cur_scale / 1.6).log().repeat_dim(0, 2);

                // Concat then prune, so that splat is nevery empty.
                // TODO: Do this the other way around when Burn fixes tensors with 0 length.
                splats.concat_splats(new_means, new_rots, new_coeffs, new_opac, new_scales);

                self.prune_points(&mut splats, split_mask.clone()).await;
            }

            if self.iter % (self.config.refine_every * self.config.reset_alpha_every) == 0 {
                self.reset_opacity(&mut splats);
            }

            self.reset_stats(splats.num_splats(), device);
            self.optim = self.opt_config.init::<B, Splats<B>>();
        }

        self.iter += 1;

        let stats = TrainStepStats {
            pred_images,
            auxes,
            loss,
            lr_mean: self.sched_mean.step(),
            iter: self.iter,
        };

        Ok((splats, stats))
    }
}
