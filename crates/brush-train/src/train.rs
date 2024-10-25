use anyhow::Result;
use brush_render::gaussian_splats::{inverse_sigmoid, Splats};
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
use tracing::trace_span;

use crate::scene::SceneView;
use crate::ssim::Ssim;

#[derive(Config)]
pub struct TrainConfig {
    #[config(default = 30000)]
    total_steps: usize,

    // period of steps where refinement is turned off
    #[config(default = 500)]
    warmup_steps: u32,

    // period of steps where gaussians are culled and densified
    #[config(default = 100)]
    refine_every: u32,

    #[config(default = 0.5)]
    stop_refine_percent: f32,

    #[config(default = 0.004)]
    reset_alpha_value: f32,

    // threshold of opacity for culling gaussians. One can set it to a lower value (e.g. 0.005) for higher quality
    #[config(default = 0.005)]
    cull_alpha_thresh: f32,

    // threshold of scale for culling huge gaussians
    #[config(default = 5.0)]
    cull_scale_thresh: f32,

    // Every this many refinement steps, reset the alpha
    #[config(default = 15)]
    reset_alpha_every_refine: u32,

    // threshold of positional gradient norm for densifying gaussians
    // TODO: Abs grad.
    #[config(default = 0.0002)]
    densify_grad_thresh: f32,

    // below this size, gaussians are *duplicated*, otherwise split.
    #[config(default = 0.005)]
    densify_size_thresh: f32,

    #[config(default = 0.2)]
    ssim_weight: f32,

    // TODO: Up this to 11 when convolutions aren't as slow anymore
    #[config(default = 5)]
    ssim_window_size: usize,

    #[config(default = true)]
    scale_mean_lr_by_extent: bool,

    // Learning rates.
    lr_mean: ExponentialLrSchedulerConfig,

    // Learning rate for the basic coefficients.
    #[config(default = 0.004)]
    lr_coeffs_dc: f64,

    // How much to divide the learning rate by for higher SH orders.
    #[config(default = 20.0)]
    lr_coeffs_sh_scale: f64,

    #[config(default = 0.05)]
    lr_opac: f64,

    #[config(default = 0.0075)]
    lr_scale: f64,

    #[config(default = 0.001)]
    lr_rotation: f64,

    #[config(default = 42)]
    seed: u64,
}

#[derive(Clone, Debug)]
pub struct SceneBatch<B: Backend> {
    pub gt_images: Tensor<B, 4>,
    pub gt_views: Vec<SceneView>,
    pub scene_extent: f64,
}

#[derive(Clone)]
pub struct RefineStats {
    pub num_split: usize,
    pub num_cloned: usize,
    pub num_transparent_pruned: usize,
    pub num_scale_pruned: usize,
}

#[derive(Clone)]
pub struct TrainStepStats<B: AutodiffBackend> {
    pub pred_images: Tensor<B, 4>,
    pub gt_images: Tensor<B, 4>,
    pub gt_views: Vec<SceneView>,
    pub auxes: Vec<RenderAux>,
    pub loss: Tensor<B, 1>,
    pub lr_mean: f64,
    pub lr_rotation: f64,
    pub lr_scale: f64,
    pub lr_coeffs: f64,
    pub lr_opac: f64,

    pub refine: Option<RefineStats>,
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

    ssim: Ssim<B>,
}

fn quaternion_vec_multiply<B: Backend>(
    quaternions: Tensor<B, 2>,
    vectors: Tensor<B, 2>,
) -> Tensor<B, 2> {
    let num_points = quaternions.dims()[0];

    // Extract components of quaternions
    let qw = quaternions.clone().slice([0..num_points, 0..1]);
    let qx = quaternions.clone().slice([0..num_points, 1..2]);
    let qy = quaternions.clone().slice([0..num_points, 2..3]);
    let qz = quaternions.clone().slice([0..num_points, 3..4]);

    // Extract components of vectors
    let vx = vectors.clone().slice([0..num_points, 0..1]);
    let vy = vectors.clone().slice([0..num_points, 1..2]);
    let vz = vectors.clone().slice([0..num_points, 2..3]);

    // Compute intermediate terms
    let term1 = qw.clone() * vx.clone() + qy.clone() * vz.clone() - qz.clone() * vy.clone();
    let term2 = qw.clone() * vy.clone() - qx.clone() * vz.clone() + qz.clone() * vx.clone();
    let term3 = qw.clone() * vz.clone() + qx.clone() * vy.clone() - qy.clone() * vx.clone();
    let term4 = qx.clone() * vx.clone() + qy.clone() * vy.clone() + qz.clone() * vz.clone();

    // Compute final result
    let rx = vx
        + (qw.clone() * term1.clone() + qx.clone() * term4.clone() - qy.clone() * term3.clone()
            + qz.clone() * term2.clone())
            * 2.0;
    let ry = vy
        + (qw.clone() * term2.clone() - qx.clone() * term3.clone()
            + qy.clone() * term4.clone()
            + qz.clone() * term1.clone())
            * 2.0;
    let rz = vz
        + (qw * term3.clone() + qx * term2.clone() - qy * term1.clone() + qz * term4.clone()) * 2.0;

    Tensor::cat(vec![rx, ry, rz], 1)
}

impl<B: AutodiffBackend> SplatTrainer<B>
where
    B::InnerBackend: Backend,
{
    pub fn new(num_points: usize, config: &TrainConfig, splats: &Splats<B>) -> Self {
        let opt_config = AdamConfig::new().with_epsilon(1e-15);
        let optim = opt_config.init::<B, Splats<B>>();

        let device = &splats.means.device();

        let ssim = Ssim::new(config.ssim_window_size, 3, device);
        Self {
            config: config.clone(),
            iter: 0,
            sched_mean: config.lr_mean.init(),
            optim,
            opt_config,
            grad_2d_accum: Tensor::zeros([num_points], device),
            xy_grad_counts: Tensor::zeros([num_points], device),
            ssim,
        }
    }

    fn reset_stats(&mut self, num_points: usize, device: &B::Device) {
        self.grad_2d_accum = Tensor::zeros([num_points], device);
        self.xy_grad_counts = Tensor::zeros([num_points], device);
    }

    pub(crate) fn reset_opacity(&self, splats: &mut Splats<B>) {
        Splats::map_param(&mut splats.raw_opacity, |op| {
            Tensor::zeros_like(&op) + inverse_sigmoid(self.config.reset_alpha_value)
        });
    }

    pub async fn step(
        &mut self,
        batch: SceneBatch<B>,
        background_color: glam::Vec3,
        splats: Splats<B>,
    ) -> Result<(Splats<B>, TrainStepStats<B>), anyhow::Error> {
        let mut splats = splats;

        let [batch_size, img_h, img_w, _] = batch.gt_images.dims();

        let (pred_images, auxes, loss) = {
            let mut renders = vec![];
            let mut auxes = vec![];

            for i in 0..batch.gt_views.len() {
                let camera = &batch.gt_views[i].camera;

                let (pred_image, aux) = splats.render(
                    camera,
                    glam::uvec2(img_w as u32, img_h as u32),
                    background_color,
                    false,
                );

                renders.push(pred_image);
                auxes.push(aux);
            }

            let pred_images = Tensor::stack(renders, 0);

            let _span = trace_span!("Calculate losses", sync_burn = true).entered();

            let pred_rgb = pred_images
                .clone()
                .slice([0..batch_size, 0..img_h, 0..img_w, 0..3]);
            let gt_rgb = batch
                .gt_images
                .clone()
                .slice([0..batch_size, 0..img_h, 0..img_w, 0..3]);

            let loss = (pred_rgb.clone() - gt_rgb.clone()).abs().mean();
            let loss = if self.config.ssim_weight > 0.0 {
                let ssim_loss = -self.ssim.ssim(pred_rgb.clone(), gt_rgb.clone()) + 1.0;
                loss * (1.0 - self.config.ssim_weight) + ssim_loss * self.config.ssim_weight
            } else {
                loss
            };

            (pred_images, auxes, loss)
        };

        let max_refine_step =
            (self.config.stop_refine_percent * self.config.total_steps as f32) as u32;

        let mut grads = trace_span!("Backward pass", sync_burn = true).in_scope(|| loss.backward());

        // TODO: Should scale lr be scales by scene scale as well?
        let (lr_mean, lr_rotation, lr_scale, lr_coeffs, lr_opac) = (
            self.sched_mean.step() * batch.scene_extent,
            self.config.lr_rotation,
            self.config.lr_scale,
            self.config.lr_coeffs_dc,
            self.config.lr_opac,
        );

        trace_span!("Housekeeping", sync_burn = true).in_scope(|| {
            // Get the xy gradient norm from the dummy tensor.
            let xy_norm_grad = Tensor::from_inner(
                splats
                    .xys_norm_dummy
                    .grad_remove(&mut grads)
                    .expect("XY gradients need to be calculated."),
            );

            // TODO: Burn really should implement +=
            if self.iter > self.config.warmup_steps {
                self.grad_2d_accum = self.grad_2d_accum.clone() + xy_norm_grad.clone();
                self.xy_grad_counts =
                    self.xy_grad_counts.clone() + xy_norm_grad.greater_elem(0.0).int();
            }
        });

        let post_step_splat = trace_span!("Optimizer step", sync_burn = true).in_scope(|| {
            let mut splats = splats.clone();
            let grad_means = GradientsParams::from_params(&mut grads, &splats, &[splats.means.id]);
            splats = self.optim.step(lr_mean, splats, grad_means);

            let grad_opac =
                GradientsParams::from_params(&mut grads, &splats, &[splats.raw_opacity.id]);
            splats = self.optim.step(lr_opac, splats, grad_opac);

            let old_coeffs = splats.sh_coeffs.val();
            let grad_coeff =
                GradientsParams::from_params(&mut grads, &splats, &[splats.sh_coeffs.id]);
            splats = self.optim.step(lr_coeffs, splats, grad_coeff);
            let num_splats = splats.num_splats();
            let sh_num = splats.sh_coeffs.dims()[1];

            // HACK: Want a lower learning rate for higher SH order.
            // This works as long as the update rule is linear.
            // (Adam Update rule is theta_{t + 1} = theta_{t} - lr * step)
            if sh_num > 1 {
                Splats::map_param(&mut splats.sh_coeffs, |coeffs| {
                    let coeff_step = coeffs.clone() - old_coeffs.clone();
                    let scaled_coeffs =
                        old_coeffs.clone() + coeff_step / self.config.lr_coeffs_sh_scale;
                    let base = coeffs.slice([0..num_splats, 0..1]);
                    let scaled = scaled_coeffs.slice([0..num_splats, 1..sh_num]);
                    Tensor::cat(vec![base, scaled], 1)
                });
            }

            let grad_rot = GradientsParams::from_params(&mut grads, &splats, &[splats.rotation.id]);
            splats = self.optim.step(lr_rotation, splats, grad_rot);

            let grad_scale =
                GradientsParams::from_params(&mut grads, &splats, &[splats.log_scales.id]);
            splats = self.optim.step(lr_scale, splats, grad_scale);

            // Make sure rotations are still valid after optimization step.
            splats.norm_rotations();
            splats
        });

        let mut refine_stats = None;

        let do_refine = self.iter < max_refine_step
            && self.iter >= self.config.warmup_steps
            && self.iter % self.config.refine_every == 1;

        splats = if !do_refine {
            // If not refining, update splat to step with gradients applied.
            post_step_splat
        } else {
            let (splats, refine) = self.refine_splats(splats, post_step_splat).await;
            refine_stats = Some(refine);
            splats
        };

        self.iter += 1;

        let stats = TrainStepStats {
            pred_images,
            gt_images: batch.gt_images,
            gt_views: batch.gt_views,
            auxes,
            loss,
            lr_mean,
            lr_rotation,
            lr_scale,
            lr_coeffs,
            lr_opac,
            refine: refine_stats,
        };

        Ok((splats, stats))
    }

    async fn refine_splats(
        &mut self,
        splats: Splats<B>,
        post_step_splat: Splats<B>,
    ) -> (Splats<B>, RefineStats) {
        let device = splats.means.device();
        let mut splats_pre_step = splats;
        let mut splats_post_step = post_step_splat.clone();

        // Otherwise, do refinement, but do the split/clone on gaussians with no grads applied.
        let grads =
            self.grad_2d_accum.clone() / self.xy_grad_counts.clone().clamp(1, i32::MAX).float();

        let big_grad_mask = grads.greater_equal_elem(self.config.densify_grad_thresh);
        let split_clone_size_mask = splats_post_step
            .scales()
            .max_dim(1)
            .squeeze(1)
            .lower_elem(self.config.densify_size_thresh);

        let mut append_means = vec![];
        let mut append_rots = vec![];
        let mut append_coeffs = vec![];
        let mut append_opac = vec![];
        let mut append_scales = vec![];

        let clone_inds = Tensor::stack::<2>(
            vec![split_clone_size_mask.clone(), big_grad_mask.clone()],
            1,
        )
        .all_dim(1)
        .squeeze::<1>(1)
        .argwhere_async()
        .await;

        // Clone splats
        let clone_count = clone_inds.dims()[0];
        if clone_count > 0 {
            let clone_inds = clone_inds.squeeze(1);
            append_means.push(splats_pre_step.means.val().select(0, clone_inds.clone()));
            append_rots.push(splats_pre_step.rotation.val().select(0, clone_inds.clone()));
            append_coeffs.push(
                splats_pre_step
                    .sh_coeffs
                    .val()
                    .select(0, clone_inds.clone()),
            );
            append_opac.push(
                splats_pre_step
                    .raw_opacity
                    .val()
                    .select(0, clone_inds.clone()),
            );
            append_scales.push(
                splats_pre_step
                    .log_scales
                    .val()
                    .select(0, clone_inds.clone()),
            );
        }

        // Split splats.
        let split_mask =
            Tensor::stack::<2>(vec![split_clone_size_mask.bool_not(), big_grad_mask], 1).all_dim(1);
        let split_inds = split_mask.clone().squeeze::<1>(1).argwhere_async().await;
        let split_count = split_inds.dims()[0];
        if split_count > 0 {
            let split_inds = split_inds.squeeze(1);

            // Some parts can be straightforwardly copied to the new splats.
            let cur_coeff = splats_post_step
                .sh_coeffs
                .val()
                .select(0, split_inds.clone());
            let cur_raw_opac = splats_post_step
                .raw_opacity
                .val()
                .select(0, split_inds.clone());
            let cur_rots = splats_post_step
                .rotation
                .val()
                .select(0, split_inds.clone());
            append_rots.push(cur_rots.clone());
            append_coeffs.push(cur_coeff.clone());
            append_opac.push(cur_raw_opac);

            // Change current scale to be lower.
            let cur_scale = splats_post_step.scales().select(0, split_inds.clone());
            Splats::map_param(&mut splats_post_step.log_scales, |m| {
                let div_scales = Tensor::zeros_like(&m).select_assign(
                    0,
                    split_inds.clone(),
                    (cur_scale.clone() / 1.6).log(),
                );
                m.mask_where(split_mask.clone(), div_scales)
            });
            // Append newer smaller scales.
            append_scales.push((cur_scale.clone() / 1.6).log());

            // Sample new position for splits.
            let cur_means = splats_pre_step.means.val().select(0, split_inds.clone());
            let samples = quaternion_vec_multiply(
                cur_rots.clone(),
                Tensor::random([split_count, 3], Distribution::Normal(0.0, 0.5), &device)
                    * cur_scale.clone(),
            );
            // Assign new means to current values.
            Splats::map_param(&mut splats_pre_step.means, |m| {
                let offset_means = Tensor::zeros_like(&m).select_assign(
                    0,
                    split_inds.clone(),
                    cur_means.clone() - samples.clone(),
                );
                m.mask_where(split_mask.clone(), offset_means)
            });

            // Append new means with offset sample.
            let samples_new = quaternion_vec_multiply(
                cur_rots.clone(),
                Tensor::random([split_count, 3], Distribution::Normal(0.0, 0.5), &device)
                    * cur_scale,
            );
            append_means.push(cur_means.clone() + samples_new);
        }

        // Do processing on splat post step.
        let mut splats = post_step_splat;

        if !append_means.is_empty() {
            let append_means = Tensor::cat(append_means, 0);
            let append_rots = Tensor::cat(append_rots, 0);
            let append_coeffs = Tensor::cat(append_coeffs, 0);
            let append_opac = Tensor::cat(append_opac, 0);
            let append_scales = Tensor::cat(append_scales, 0);

            concat_splats(
                &mut splats,
                append_means,
                append_rots,
                append_coeffs,
                append_opac,
                append_scales,
            );
        }

        // Do some more processing. Important to do this last as otherwise you might mess up the correspondence
        // of gradient <-> splat.

        let start_count = splats.num_splats();

        // Remove barely visible gaussians.
        let alpha_mask = splats.opacity().lower_elem(self.config.cull_alpha_thresh);
        prune_points(&mut splats, alpha_mask).await;

        let alpha_pruned = start_count - splats.num_splats();

        // Delete Gaussians with too large of a radius in world-units.
        let scale_mask = splats
            .log_scales
            .val()
            .exp()
            .max_dim(1)
            .squeeze(1)
            .greater_elem(self.config.cull_scale_thresh);
        prune_points(&mut splats, scale_mask).await;

        let scale_pruned = start_count - splats.num_splats();

        let refine_step = self.iter / self.config.refine_every;
        if refine_step % self.config.reset_alpha_every_refine == 0 {
            self.reset_opacity(&mut splats);
        }

        // Stats don't line up anymore so have to reset them.
        self.reset_stats(splats.num_splats(), &device);

        // TODO: Want to do state surgery and keep momenta for splats.
        self.optim = self.opt_config.init();

        let stats = RefineStats {
            num_split: split_count,
            num_cloned: clone_count,
            num_transparent_pruned: alpha_pruned,
            num_scale_pruned: scale_pruned,
        };

        (splats, stats)
    }
}

// Prunes points based on the given mask.
//
// Args:
//   mask: bool[n]. If True, prune this Gaussian.
pub async fn prune_points<B: AutodiffBackend>(splats: &mut Splats<B>, prune: Tensor<B, 1, Bool>) {
    // bool[n]. If True, delete these Gaussians.
    let prune_count = prune.dims()[0];

    if prune_count == 0 {
        return;
    }

    let valid_inds = prune.bool_not().argwhere_async().await.squeeze(1);
    let start_splats = splats.num_splats();
    let new_points = valid_inds.dims()[0];

    if new_points < start_splats {
        splats.means = splats
            .means
            .clone()
            .map(|x| Tensor::from_inner(x.select(0, valid_inds.clone()).inner()).require_grad());
        splats.sh_coeffs = splats
            .sh_coeffs
            .clone()
            .map(|x| Tensor::from_inner(x.select(0, valid_inds.clone()).inner()).require_grad());
        splats.rotation = splats
            .rotation
            .clone()
            .map(|x| Tensor::from_inner(x.select(0, valid_inds.clone()).inner()).require_grad());
        splats.raw_opacity = splats
            .raw_opacity
            .clone()
            .map(|x| Tensor::from_inner(x.select(0, valid_inds.clone()).inner()).require_grad());
        splats.log_scales = splats
            .log_scales
            .clone()
            .map(|x| Tensor::from_inner(x.select(0, valid_inds.clone()).inner()).require_grad());
    }
}

pub fn concat_splats<B: AutodiffBackend>(
    splats: &mut Splats<B>,
    means: Tensor<B, 2>,
    rotations: Tensor<B, 2>,
    sh_coeffs: Tensor<B, 3>,
    raw_opacities: Tensor<B, 1>,
    log_scales: Tensor<B, 2>,
) {
    Splats::map_param(&mut splats.means, |x| {
        Tensor::cat(vec![x, means.clone()], 0)
    });
    Splats::map_param(&mut splats.rotation, |x| {
        Tensor::cat(vec![x, rotations.clone()], 0)
    });
    Splats::map_param(&mut splats.sh_coeffs, |x| {
        Tensor::cat(vec![x, sh_coeffs.clone()], 0)
    });
    Splats::map_param(&mut splats.raw_opacity, |x| {
        Tensor::cat(vec![x, raw_opacities.clone()], 0)
    });
    Splats::map_param(&mut splats.log_scales, |x| {
        Tensor::cat(vec![x, log_scales.clone()], 0)
    });
}
